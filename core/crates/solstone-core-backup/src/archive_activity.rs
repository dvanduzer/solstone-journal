// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One original activity reservation and an archive's nonrenewable admission.

use std::fmt;
use std::io;
use std::sync::Arc;

use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::activity_obligations::observe_obligations;
use solstone_core_journal_io::media::{ActivityCarrierPreparationFailure, RootActivityCarrier};

use crate::archive_admission::{AdmissionError, ArchiveBudget, ArchiveConfiguration, Clock};

/// A retained reservation. Cloning its native Arc never admits another operation.
#[must_use = "Retain the original activity until its actual owning work has ended"]
pub(crate) struct ArchiveActivity {
    carrier: Arc<RootActivityCarrier>,
}

#[must_use = "Preparation failure may retain an original lease and partial reservation"]
pub struct ActivityPreparationFailure {
    inner: Box<ActivityPreparationFailureInner>,
}

struct ActivityPreparationFailureInner {
    error: io::Error,
    root: Arc<JournalRoot>,
    lease: Option<Arc<ActivityLease>>,
    partial: Option<ActivityCarrierPreparationFailure>,
}

impl fmt::Debug for ActivityPreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivityPreparationFailure")
            .field("kind", &self.inner.error.kind())
            .finish_non_exhaustive()
    }
}
impl fmt::Display for ActivityPreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.error.fmt(f)
    }
}
impl std::error::Error for ActivityPreparationFailure {}

impl ActivityPreparationFailure {
    pub fn error(&self) -> &io::Error {
        &self.inner.error
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        &self.inner.root
    }
    pub fn activity(&self) -> Option<&Arc<ActivityLease>> {
        self.inner.lease.as_ref()
    }
    pub fn partial(&self) -> Option<&ActivityCarrierPreparationFailure> {
        self.inner.partial.as_ref()
    }
}

impl ArchiveActivity {
    fn prepare(root: Arc<JournalRoot>) -> Result<Self, ActivityPreparationFailure> {
        let mut failure = ActivityPreparationFailureInner {
            error: io::Error::other("activity preparation failed"),
            root: Arc::clone(&root),
            lease: None,
            partial: None,
        };
        let lease = match ActivityLease::try_acquire(Arc::clone(&root)) {
            Ok(lease) => lease,
            Err(error) => {
                failure.error = io::Error::other(error.to_string());
                return Err(ActivityPreparationFailure {
                    inner: Box::new(failure),
                });
            }
        };
        failure.lease = Some(Arc::clone(&lease));
        let census = match observe_obligations(&root) {
            Ok(census) => census,
            Err(error) => {
                failure.error = io::Error::other(error.to_string());
                return Err(ActivityPreparationFailure {
                    inner: Box::new(failure),
                });
            }
        };
        if !census.entries().is_empty() {
            failure.error = io::Error::other("unresolved journal activity");
            return Err(ActivityPreparationFailure {
                inner: Box::new(failure),
            });
        }
        match RootActivityCarrier::prepare(lease) {
            Ok(carrier) => Ok(Self {
                carrier: Arc::new(carrier),
            }),
            Err(partial) => {
                failure.error = io::Error::new(partial.error().kind(), partial.error().to_string());
                failure.partial = Some(partial);
                Err(ActivityPreparationFailure {
                    inner: Box::new(failure),
                })
            }
        }
    }

    pub fn carrier(&self) -> &Arc<RootActivityCarrier> {
        &self.carrier
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.carrier.activity()
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.carrier.journal_root()
    }
    pub fn revalidate(&self) -> io::Result<()> {
        self.journal_root()
            .revalidate_canonical_binding()
            .map_err(io::Error::other)?;
        self.activity().revalidate().map_err(io::Error::other)?;
        self.carrier.revalidate()
    }
}

/// One unused archive admission. Its lifetime cannot be extracted or rebound.
#[must_use = "Move the original operation through archive and read admission"]
pub struct PreparedArchiveOperation<'a> {
    pub(crate) activity: ArchiveActivity,
    pub(crate) configuration: ArchiveConfiguration,
    pub(crate) budget: ArchiveBudget,
    pub(crate) clock: &'a dyn Clock,
}

#[must_use = "An archive preparation refusal retains its original admission"]
pub struct ArchiveOperationPreparationFailure<'a> {
    inner: Box<ArchiveOperationPreparationFailureInner<'a>>,
}

struct ArchiveOperationPreparationFailureInner<'a> {
    reason: AdmissionError,
    root: Arc<JournalRoot>,
    activity: Option<ArchiveActivity>,
    preparation: Option<ActivityPreparationFailure>,
    _budget: ArchiveBudget,
    _clock: &'a dyn Clock,
}
impl fmt::Debug for ArchiveOperationPreparationFailure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveOperationPreparationFailure")
            .field("reason", &self.inner.reason)
            .finish_non_exhaustive()
    }
}
impl fmt::Display for ArchiveOperationPreparationFailure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.reason.fmt(f)
    }
}
impl std::error::Error for ArchiveOperationPreparationFailure<'_> {}
impl ArchiveOperationPreparationFailure<'_> {
    pub fn reason(&self) -> AdmissionError {
        self.inner.reason
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        &self.inner.root
    }
    pub fn carrier(&self) -> Option<&Arc<RootActivityCarrier>> {
        self.inner.activity.as_ref().map(ArchiveActivity::carrier)
    }
    pub fn preparation(&self) -> Option<&ActivityPreparationFailure> {
        self.inner.preparation.as_ref()
    }
}

impl<'a> PreparedArchiveOperation<'a> {
    pub fn prepare(
        root: Arc<JournalRoot>,
        budget: ArchiveBudget,
        clock: &'a dyn Clock,
    ) -> Result<Self, ArchiveOperationPreparationFailure<'a>> {
        let mut failure = ArchiveOperationPreparationFailureInner {
            reason: AdmissionError::JournalIdentityChanged,
            root: Arc::clone(&root),
            activity: None,
            preparation: None,
            _budget: budget,
            _clock: clock,
        };
        if let Err(reason) = failure._budget.remaining(clock) {
            failure.reason = reason;
            return Err(ArchiveOperationPreparationFailure {
                inner: Box::new(failure),
            });
        }
        let activity = match ArchiveActivity::prepare(root) {
            Ok(activity) => activity,
            Err(preparation) => {
                failure.preparation = Some(preparation);
                return Err(ArchiveOperationPreparationFailure {
                    inner: Box::new(failure),
                });
            }
        };
        let configuration = match ArchiveConfiguration::capture(activity.journal_root()) {
            Ok(configuration) => configuration,
            Err(reason) => {
                failure.reason = reason;
                failure.activity = Some(activity);
                return Err(ArchiveOperationPreparationFailure {
                    inner: Box::new(failure),
                });
            }
        };
        Ok(Self {
            activity,
            configuration,
            budget: failure._budget,
            clock,
        })
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.activity.activity()
    }
    pub fn carrier(&self) -> &Arc<RootActivityCarrier> {
        self.activity.carrier()
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.activity.journal_root()
    }
    pub fn configuration(&self) -> &ArchiveConfiguration {
        &self.configuration
    }
    pub fn budget(&self) -> &ArchiveBudget {
        &self.budget
    }
    pub fn clock(&self) -> &'a dyn Clock {
        self.clock
    }
    pub fn cap_credential_expiry(&mut self, expiry: i64) {
        self.budget.cap_credential_expiry(expiry);
    }
    pub fn revalidate(&self) -> Result<(), AdmissionError> {
        self.budget.remaining(self.clock)?;
        self.activity
            .revalidate()
            .map_err(|_| AdmissionError::JournalIdentityChanged)?;
        self.configuration.revalidate(self.journal_root())
    }
}
