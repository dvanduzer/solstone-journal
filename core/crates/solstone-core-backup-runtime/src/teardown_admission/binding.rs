// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Local consumed phases. These values do not certify remote settlement.

use super::*;
use solstone_core_backup::teardown_configuration::{
    BindingFinalization, BindingFinalizationRefusal, BindingRemovalReceipt,
};

mod finish;
pub use finish::TeardownFinishAdmission;

pub enum TeardownBindingRefusal {
    Admission(TeardownAdmissionRefusal),
    Binding(BindingFinalizationRefusal),
    Removal,
}

impl std::fmt::Debug for TeardownBindingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(refusal) => f.debug_tuple("Admission").field(refusal).finish(),
            Self::Binding(refusal) => f.debug_tuple("Binding").field(refusal).finish(),
            Self::Removal => f.write_str("Removal"),
        }
    }
}

enum BindingFailureOwner {
    Cleared(Box<ClearedTeardownAdmission>),
    Guarded(Box<BindingTeardownAdmission>),
}

/// Original owners and actual receipts survive every returned local refusal.
pub struct TeardownBindingFailure {
    owner: BindingFailureOwner,
    refusal: TeardownBindingRefusal,
}

impl TeardownBindingFailure {
    pub(super) fn cleared(
        owner: ClearedTeardownAdmission,
        refusal: TeardownBindingRefusal,
    ) -> Self {
        Self {
            owner: BindingFailureOwner::Cleared(Box::new(owner)),
            refusal,
        }
    }
    fn guarded(owner: BindingTeardownAdmission, refusal: TeardownBindingRefusal) -> Self {
        Self {
            owner: BindingFailureOwner::Guarded(Box::new(owner)),
            refusal,
        }
    }
    fn cleared_owner(&self) -> &ClearedTeardownAdmission {
        match &self.owner {
            BindingFailureOwner::Cleared(owner) => owner,
            BindingFailureOwner::Guarded(owner) => &owner.cleared,
        }
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.cleared_owner().journal_root()
    }
    pub fn carrier(&self) -> &Arc<RootTeardownCarrier> {
        self.cleared_owner().carrier()
    }
    pub fn publication(&self) -> &Arc<BoundConfigPublication> {
        self.cleared_owner().publication_receipt()
    }
    pub fn binding_receipt(&self) -> Option<&Arc<BindingRemovalReceipt>> {
        match &self.owner {
            BindingFailureOwner::Cleared(_) => None,
            BindingFailureOwner::Guarded(owner) => owner.binding.receipt(),
        }
    }
    pub fn refusal(&self) -> &TeardownBindingRefusal {
        &self.refusal
    }
}

impl std::fmt::Debug for TeardownBindingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownBindingFailure")
            .field("refusal", &self.refusal)
            .field("binding_receipt", &self.binding_receipt().is_some())
            .finish()
    }
}

/// Holds config→binding guards before the one local binding attempt.
pub struct BindingTeardownAdmission {
    cleared: ClearedTeardownAdmission,
    binding: BindingFinalization,
}

impl ClearedTeardownAdmission {
    /// Acquire original local guards only after the actual confirmed clear.
    pub fn begin_binding_finalization(
        self,
        reader: &mut TeardownReader<'_>,
    ) -> Result<BindingTeardownAdmission, TeardownBindingFailure> {
        if let Err(refusal) = self.check(reader) {
            return Err(TeardownBindingFailure::cleared(
                self,
                TeardownBindingRefusal::Admission(refusal),
            ));
        }
        let binding = match self
            .configuration()
            .begin_binding_finalization(self.publication())
        {
            Ok(binding) => binding,
            Err(refusal) => {
                return Err(TeardownBindingFailure::cleared(
                    self,
                    TeardownBindingRefusal::Binding(refusal),
                ));
            }
        };
        let guarded = BindingTeardownAdmission {
            cleared: self,
            binding,
        };
        if let Err(refusal) = guarded.cleared.check(reader) {
            return Err(TeardownBindingFailure::guarded(
                guarded,
                TeardownBindingRefusal::Admission(refusal),
            ));
        }
        Ok(guarded)
    }
}

impl BindingTeardownAdmission {
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.cleared.journal_root()
    }
    pub fn carrier(&self) -> &Arc<RootTeardownCarrier> {
        self.cleared.carrier()
    }
    pub fn publication(&self) -> &Arc<BoundConfigPublication> {
        self.cleared.publication_receipt()
    }

    /// A fresh original-view callback precedes the one actual native attempt.
    pub fn remove_binding(
        mut self,
        reader: &mut TeardownReader<'_>,
    ) -> Result<BoundTeardownAdmission, TeardownBindingFailure> {
        if let Err(refusal) = self.cleared.check(reader) {
            return Err(TeardownBindingFailure::guarded(
                self,
                TeardownBindingRefusal::Admission(refusal),
            ));
        }
        if !self.binding.remove_binding().complete() {
            return Err(TeardownBindingFailure::guarded(
                self,
                TeardownBindingRefusal::Removal,
            ));
        }
        let bound = BoundTeardownAdmission { guarded: self };
        if let Err(refusal) = bound.check(reader) {
            return Err(TeardownBindingFailure::guarded(bound.guarded, refusal));
        }
        Ok(bound)
    }
}

/// Exact confirmed local transitions with all guards retained. No finish factory.
pub struct BoundTeardownAdmission {
    guarded: BindingTeardownAdmission,
}

impl BoundTeardownAdmission {
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.guarded.journal_root()
    }
    pub fn carrier(&self) -> &Arc<RootTeardownCarrier> {
        self.guarded.carrier()
    }
    pub fn teardown_lease(&self) -> &Arc<TeardownLease> {
        self.guarded.cleared.teardown_lease()
    }
    pub fn publication(&self) -> &Arc<BoundConfigPublication> {
        self.guarded.publication()
    }
    pub fn binding_receipt(&self) -> &Arc<BindingRemovalReceipt> {
        self.guarded
            .binding
            .receipt()
            .expect("confirmed native attempt")
    }
    pub fn revalidate(&self) -> Result<(), TeardownBindingRefusal> {
        self.teardown_lease()
            .revalidate()
            .map_err(|error| TeardownBindingRefusal::Admission(activity_refusal(error)))?;
        self.carrier().revalidate().map_err(|_| {
            TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::CarrierUnavailable)
        })?;
        self.guarded
            .binding
            .revalidate_removed()
            .map_err(TeardownBindingRefusal::Binding)?;
        self.carrier().revalidate().map_err(|_| {
            TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::CarrierUnavailable)
        })?;
        self.teardown_lease()
            .revalidate()
            .map_err(|error| TeardownBindingRefusal::Admission(activity_refusal(error)))
    }
    /// Recheck exact candidate/absence, not the obsolete binding-present predicate.
    pub fn check(&self, reader: &mut TeardownReader<'_>) -> Result<(), TeardownBindingRefusal> {
        self.revalidate()?;
        let result = reader(
            self.journal_root(),
            self.guarded.cleared.configuration(),
            Some(self.carrier().observation()),
        );
        self.revalidate()?;
        result.map_err(|refusal| {
            TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::Reader(refusal))
        })
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(unix)]
mod tests;
