// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::activity::{ActivityLease, TeardownLease};

/// Independent facts about preparation. Observations grant no cleanup authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivityCarrierPreparationReceipt {
    stage_durability: Durability,
    root_durability: Durability,
    confirmation: Confirmation,
}

impl ActivityCarrierPreparationReceipt {
    pub fn stage_durability(&self) -> Durability {
        self.stage_durability
    }

    pub fn root_durability(&self) -> Durability {
        self.root_durability
    }

    pub fn confirmation(&self) -> Confirmation {
        self.confirmation
    }
}

struct CarrierOwner {
    activity: StageLease,
    stage: Option<MediaStage>,
}

impl CarrierOwner {
    fn prepare(activity: StageLease) -> (Self, ActivityCarrierPreparationReceipt, io::Result<()>) {
        let mut owner = CarrierOwner {
            activity,
            stage: None,
        };
        let mut receipt = ActivityCarrierPreparationReceipt {
            stage_durability: Durability::Unproven,
            root_durability: Durability::Unproven,
            confirmation: Confirmation::Unverified,
        };
        let result = (|| {
            owner
                .activity
                .revalidate()
                .map_err(crate::activity::ActivityAdmissionError::into_io)?;
            let root = MediaDirectory::open(owner.activity.journal_root().clone(), Path::new(""))?;
            match MediaStage::create_with_lease(root, owner.activity.clone()) {
                Ok(stage) => owner.stage = Some(stage),
                Err(failure) => {
                    owner.stage = failure.owned;
                    return Err(failure.error);
                }
            }
            owner.revalidate_empty()?;
            step(MediaPrimitive::CarrierStageSync)?;
            owner.revalidate_empty()?;
            owner
                .stage
                .as_ref()
                .and_then(|stage| stage.directory.as_ref())
                .expect("validated directory retained")
                .inner
                .sync()?;
            receipt.stage_durability = Durability::Synced;
            owner.revalidate_empty()?;
            step(MediaPrimitive::CarrierRootSync)?;
            owner.revalidate_empty()?;
            owner
                .stage
                .as_ref()
                .expect("validated stage retained")
                .parent
                .inner
                .sync()?;
            receipt.root_durability = Durability::Synced;
            step(MediaPrimitive::CarrierConfirmation)?;
            owner.revalidate_empty()?;
            receipt.confirmation = Confirmation::Confirmed;
            Ok(())
        })();
        (owner, receipt, result)
    }

    fn activity(&self) -> &Arc<ActivityLease> {
        match &self.activity {
            StageLease::Activity(lease) => lease,
            StageLease::Teardown(_) => {
                unreachable!("private activity wrapper retains activity role")
            }
        }
    }

    fn teardown(&self) -> &Arc<TeardownLease> {
        match &self.activity {
            StageLease::Teardown(lease) => lease,
            StageLease::Activity(_) => {
                unreachable!("private teardown wrapper retains teardown role")
            }
        }
    }

    fn revalidate_empty(&self) -> io::Result<()> {
        self.activity
            .revalidate()
            .map_err(crate::activity::ActivityAdmissionError::into_io)?;
        let stage = self.stage.as_ref().ok_or_else(changed)?;
        if stage.removed {
            return Err(changed());
        }
        stage.parent.revalidate()?;
        let directory = stage.directory.as_ref().ok_or_else(changed)?;
        directory.revalidate()?;
        if directory.entries(0)?.is_none() {
            return Err(invalid("activity reservation is not empty"));
        }
        stage.parent.revalidate()?;
        self.activity
            .revalidate()
            .map_err(crate::activity::ActivityAdmissionError::into_io)
    }

    fn retire(&mut self) -> ActivityCarrierRetirementReceipt {
        let mut receipt = ActivityCarrierRetirementReceipt {
            effect: ActivityCarrierRetirementEffect::NotAttempted,
            root_durability: Durability::Unproven,
            confirmation: Confirmation::Unverified,
            failures: Vec::new(),
        };
        // An uncertain creation (stage without an admitted directory) is held.
        if let Err(error) = self.revalidate_empty() {
            receipt.failures.push(error);
            return receipt;
        }
        let stage = self.stage.as_mut().expect("validated stage retained");
        receipt.effect = ActivityCarrierRetirementEffect::Attempted;
        if let Err(error) = stage.cleanup_exact() {
            receipt.failures.push(error);
            return receipt;
        }
        receipt.effect = ActivityCarrierRetirementEffect::Removed;
        let flush = step(MediaPrimitive::CarrierRetirementRootSync)
            .and_then(|()| stage.parent.revalidate())
            .and_then(|()| stage.parent.inner.sync());
        match flush {
            Ok(()) => receipt.root_durability = Durability::Synced,
            Err(error) => receipt.failures.push(error),
        }
        // A flush error does not erase an independently confirmable removal.
        let confirmation = (|| {
            step(MediaPrimitive::CarrierRetirementConfirmation)?;
            stage.revalidate_activity()?;
            stage.parent.revalidate()?;
            platform::ensure_absent(&stage.parent.inner, &stage.name)?;
            stage.parent.revalidate()?;
            stage.revalidate_activity()
        })();
        match confirmation {
            Ok(()) => receipt.confirmation = Confirmation::Confirmed,
            Err(error) => receipt.failures.push(error),
        }
        receipt
    }
}

/// An empty root reservation made durable before journal-bound remote effects.
/// Drop closes handles only. Discovered names and saved identities cannot mint one.
pub struct RootActivityCarrier {
    owner: CarrierOwner,
    preparation: ActivityCarrierPreparationReceipt,
}

impl RootActivityCarrier {
    /// Observes this actual empty reservation and persistent lock. The result is
    /// immutable data; it never certifies recovery, cooperation or settlement.
    pub fn observe_recovery_binding(
        &self,
        origin: &crate::archive_origin::LocalArchiveOrigin,
        repository_id: &str,
    ) -> Result<
        crate::archive_recovery::ArchiveRecoveryBinding,
        crate::archive_recovery::RecoveryBindingObservationError,
    > {
        use crate::archive_recovery::RecoveryBindingObservationError as E;
        #[cfg(target_os = "linux")]
        {
            if !Arc::ptr_eq(origin.root(), self.journal_root()) {
                return Err(E::Native(invalid(
                    "origin belongs to another original root",
                )));
            }
            origin.revalidate().map_err(E::Origin)?;
            self.activity().revalidate().map_err(E::Activity)?;
            let matched = self
                .observation()
                .validate(self.journal_root())
                .map_err(E::Native)?;
            let lock = self
                .activity()
                .recovery_lock_observation()
                .map_err(E::Activity)?;
            let name = matched
                .name
                .to_str()
                .ok_or_else(|| E::Native(invalid("invalid reservation name")))?;
            let binding = crate::archive_recovery::ArchiveRecoveryBinding::observed(
                origin.evidence().clone(),
                name.into(),
                matched.identity,
                lock.clone(),
                repository_id,
            )
            .map_err(E::Native)?;
            origin.revalidate().map_err(E::Origin)?;
            if self
                .observation()
                .validate(self.journal_root())
                .map_err(E::Native)?
                != matched
                || self
                    .activity()
                    .recovery_lock_observation()
                    .map_err(E::Activity)?
                    != lock
            {
                return Err(E::Native(changed()));
            }
            self.activity().revalidate().map_err(E::Activity)?;
            Ok(binding)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (origin, repository_id);
            Err(E::Unsupported)
        }
    }

    /// Caller must freshly refuse existing unresolved obligations under this
    /// original activity lease before preparing a new operation. This native
    /// leaf cannot certify the owning catalog/intent/config admission.
    pub fn prepare(
        activity: Arc<ActivityLease>,
    ) -> Result<Self, ActivityCarrierPreparationFailure> {
        let (owner, receipt, result) = CarrierOwner::prepare(StageLease::Activity(activity));
        match result {
            Ok(()) => Ok(Self {
                owner,
                preparation: receipt,
            }),
            Err(error) => Err(ActivityCarrierPreparationFailure {
                error,
                receipt,
                owner,
            }),
        }
    }

    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.activity()
    }

    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.activity.journal_root()
    }

    pub fn preparation(&self) -> &ActivityCarrierPreparationReceipt {
        &self.preparation
    }

    pub fn diagnostic_path(&self) -> PathBuf {
        self.owner
            .stage
            .as_ref()
            .expect("prepared stage")
            .diagnostic_path()
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.owner.revalidate_empty()
    }

    pub fn observation(&self) -> RootCarrierObservation<'_> {
        RootCarrierObservation {
            owner: ObservationOwner::Live(&self.owner),
        }
    }

    /// Requires actual settlement of every child and HTTP writer and qualified
    /// cooperation/quiescence of every stage-namespace actor. Unknown holds this
    /// call. No native receipt proves those owning-runtime preconditions.
    ///
    /// Unix final check-to-rmdir can remove an uncooperating empty replacement;
    /// this method is not unconditional protection against that namespace race.
    pub fn retire_after_settlement(mut self) -> ActivityCarrierRetirement<Self> {
        let receipt = self.owner.retire();
        ActivityCarrierRetirement {
            owned: self,
            receipt,
        }
    }
}

/// Preparation failure retains partial reservations and the original lease.
pub struct ActivityCarrierPreparationFailure {
    error: io::Error,
    receipt: ActivityCarrierPreparationReceipt,
    owner: CarrierOwner,
}

impl ActivityCarrierPreparationFailure {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn receipt(&self) -> &ActivityCarrierPreparationReceipt {
        &self.receipt
    }

    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.activity()
    }

    pub fn diagnostic_path(&self) -> Option<PathBuf> {
        self.owner.stage.as_ref().map(MediaStage::diagnostic_path)
    }

    /// Requires the same actual settlement and qualified namespace cooperation
    /// as RootActivityCarrier::retire_after_settlement, including prelaunch
    /// failures. Unknown cooperation holds cleanup; Drop never unlinks.
    pub fn retire_after_settlement(mut self) -> ActivityCarrierRetirement<Self> {
        let receipt = self.owner.retire();
        ActivityCarrierRetirement {
            owned: self,
            receipt,
        }
    }
}

impl std::fmt::Debug for ActivityCarrierPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityCarrierPreparationFailure")
            .field("error", &self.error)
            .field("receipt", &self.receipt)
            .finish_non_exhaustive()
    }
}

/// Empty durable root reservation retaining the original teardown admission.
/// It exposes no writable media stage. Drop closes only; it never retires.
pub struct RootTeardownCarrier {
    owner: CarrierOwner,
    preparation: ActivityCarrierPreparationReceipt,
}

impl RootTeardownCarrier {
    /// Existing obligations must be refused freshly under this original lease.
    /// This leaf certifies no owning config, catalog or settlement policy.
    pub fn prepare(
        teardown: Arc<TeardownLease>,
    ) -> Result<Self, TeardownCarrierPreparationFailure> {
        let (owner, receipt, result) = CarrierOwner::prepare(StageLease::Teardown(teardown));
        match result {
            Ok(()) => Ok(Self {
                owner,
                preparation: receipt,
            }),
            Err(error) => Err(TeardownCarrierPreparationFailure {
                error,
                receipt,
                owner,
            }),
        }
    }

    pub fn teardown(&self) -> &Arc<TeardownLease> {
        self.owner.teardown()
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.activity.journal_root()
    }
    pub fn preparation(&self) -> &ActivityCarrierPreparationReceipt {
        &self.preparation
    }
    pub fn diagnostic_path(&self) -> PathBuf {
        self.owner
            .stage
            .as_ref()
            .expect("prepared stage")
            .diagnostic_path()
    }
    pub fn revalidate(&self) -> io::Result<()> {
        self.owner.revalidate_empty()
    }
    pub fn observation(&self) -> RootCarrierObservation<'_> {
        RootCarrierObservation {
            owner: ObservationOwner::Live(&self.owner),
        }
    }

    /// Requires actual child/HTTP settlement and qualified namespace cooperation,
    /// including the documented Unix final check-to-rmdir limitation. Unknown
    /// cooperation holds this call; the receipt cannot establish it.
    pub fn retire_after_settlement(mut self) -> ActivityCarrierRetirement<Self> {
        let receipt = self.owner.retire();
        ActivityCarrierRetirement {
            owned: self,
            receipt,
        }
    }
}

/// Retains original teardown ownership and every partial reservation on failure.
pub struct TeardownCarrierPreparationFailure {
    error: io::Error,
    receipt: ActivityCarrierPreparationReceipt,
    owner: CarrierOwner,
}

impl TeardownCarrierPreparationFailure {
    pub fn error(&self) -> &io::Error {
        &self.error
    }
    pub fn receipt(&self) -> &ActivityCarrierPreparationReceipt {
        &self.receipt
    }
    pub fn teardown(&self) -> &Arc<TeardownLease> {
        self.owner.teardown()
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.activity.journal_root()
    }
    pub fn diagnostic_path(&self) -> Option<PathBuf> {
        self.owner.stage.as_ref().map(MediaStage::diagnostic_path)
    }

    /// Same actual settlement/cooperation preconditions as the prepared carrier,
    /// even for prelaunch failure. Unknown holds; Drop does not unlink.
    pub fn retire_after_settlement(mut self) -> ActivityCarrierRetirement<Self> {
        let receipt = self.owner.retire();
        ActivityCarrierRetirement {
            owned: self,
            receipt,
        }
    }
}

impl std::fmt::Debug for TeardownCarrierPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownCarrierPreparationFailure")
            .field("error", &self.error)
            .field("receipt", &self.receipt)
            .finish_non_exhaustive()
    }
}

/// Borrow of one actual prepared root carrier. No saved name or identity can
/// construct it; the owner and original role remain borrowed throughout census.
pub struct RootCarrierObservation<'a> {
    owner: ObservationOwner<'a>,
}

enum ObservationOwner<'a> {
    Live(&'a CarrierOwner),
    Recovered(&'a crate::archive_recovery::RecoveredArchiveReservation),
}

#[derive(Eq, PartialEq)]
pub(crate) struct CarrierMatch {
    pub(crate) name: OsString,
    pub(crate) identity: MediaIdentity,
}

impl RootCarrierObservation<'_> {
    pub(crate) fn recovered(
        owner: &crate::archive_recovery::RecoveredArchiveReservation,
    ) -> RootCarrierObservation<'_> {
        RootCarrierObservation {
            owner: ObservationOwner::Recovered(owner),
        }
    }

    pub(crate) fn validate(&self, root: &Arc<JournalRoot>) -> io::Result<CarrierMatch> {
        let owner = match self.owner {
            ObservationOwner::Recovered(owner) => {
                let (name, identity) = owner.match_root(root)?;
                return Ok(CarrierMatch { name, identity });
            }
            ObservationOwner::Live(owner) => owner,
        };
        if !Arc::ptr_eq(root, owner.activity.journal_root()) {
            return Err(invalid("carrier belongs to another original root"));
        }
        owner.revalidate_empty()?;
        let stage = owner.stage.as_ref().ok_or_else(changed)?;
        if stage.parent.object_identity() != root.identity() {
            return Err(invalid("carrier is not a root reservation"));
        }
        let matched = CarrierMatch {
            name: stage.name.clone(),
            identity: stage.directory.as_ref().ok_or_else(changed)?.identity(),
        };
        owner.revalidate_empty()?;
        Ok(matched)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityCarrierRetirementEffect {
    NotAttempted,
    Attempted,
    /// Removal returned success under the owning cooperation precondition.
    /// This fact does not establish that precondition or settle any writer.
    Removed,
}

/// Actual removal, durability and confirmation remain separate facts.
#[derive(Debug)]
pub struct ActivityCarrierRetirementReceipt {
    effect: ActivityCarrierRetirementEffect,
    root_durability: Durability,
    confirmation: Confirmation,
    failures: Vec<io::Error>,
}

impl ActivityCarrierRetirementReceipt {
    pub(crate) fn not_attempted() -> Self {
        Self {
            effect: ActivityCarrierRetirementEffect::NotAttempted,
            root_durability: Durability::Unproven,
            confirmation: Confirmation::Unverified,
            failures: Vec::new(),
        }
    }

    pub fn effect(&self) -> ActivityCarrierRetirementEffect {
        self.effect
    }

    pub fn root_durability(&self) -> Durability {
        self.root_durability
    }

    pub fn confirmation(&self) -> Confirmation {
        self.confirmation
    }

    pub fn failures(&self) -> &[io::Error] {
        &self.failures
    }
}

// Finite adapter for the already physically retained recovered owner. No saved
// name/identity or public MediaDirectory method can enter this mutation.
#[cfg(target_os = "linux")]
pub(crate) fn retire_recovered(
    owner: &crate::archive_recovery::RecoveredArchiveReservation,
) -> ActivityCarrierRetirementReceipt {
    let mut receipt = ActivityCarrierRetirementReceipt::not_attempted();
    if let Err(error) = owner.revalidate() {
        receipt.failures.push(io::Error::other(error));
        return receipt;
    }
    let (parent, directory, name) = owner.retirement_directories();
    receipt.effect = ActivityCarrierRetirementEffect::Attempted;
    if let Err(error) = platform::remove_directory(&parent.inner, name, &directory.inner) {
        receipt.failures.push(error);
        return receipt;
    }
    receipt.effect = ActivityCarrierRetirementEffect::Removed;
    let flush = (|| {
        step(MediaPrimitive::CarrierRetirementRootSync)?;
        owner.revalidate_after_removal().map_err(io::Error::other)?;
        parent.inner.sync()
    })();
    match flush {
        Ok(()) => receipt.root_durability = Durability::Synced,
        Err(error) => receipt.failures.push(error),
    }
    let confirmation = (|| {
        step(MediaPrimitive::CarrierRetirementConfirmation)?;
        owner.revalidate_after_removal().map_err(io::Error::other)?;
        platform::ensure_absent(&parent.inner, name)?;
        owner.revalidate_after_removal().map_err(io::Error::other)
    })();
    match confirmation {
        Ok(()) => receipt.confirmation = Confirmation::Confirmed,
        Err(error) => receipt.failures.push(error),
    }
    receipt
}

/// Keeps the original capability and lease even after a known removal.
#[must_use = "Handle retirement facts and retained ownership independently"]
pub struct ActivityCarrierRetirement<T> {
    pub owned: T,
    pub receipt: ActivityCarrierRetirementReceipt,
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
