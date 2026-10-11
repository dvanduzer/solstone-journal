// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Consume one actual confirmed recovery result, preserving every earlier effect.

use super::*;
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::activity_obligations::{NativeObligation, RecoveryObservationError};
use solstone_core_journal_io::archive_recovery::{
    RecoveredArchiveRetirement, RecoveryAdmissionError,
};
use solstone_core_journal_io::media::{
    Durability, MediaObservation, MediaRead, MediaRemoval, MediaStage, OwnedMediaFailure,
};
use solstone_core_retention::Register;
use solstone_core_retention::archive_catalog::JournalCatalog;
use solstone_core_retention::restore_intent::RecoveryDecision;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveredRetirementState {
    Held,
    Unknown,
    Finished,
}

#[derive(Debug)]
pub enum RecoveredRetirementRefusal {
    Unsupported,
    PriorHeld,
    Physical(RecoveryAdmissionError),
    Population,
    Intent(RepairHold),
    Prepared { reason: RepairHold, detail: String },
    Claims(io::Error),
    Destination(io::Error),
    Store,
    Catalog(String),
    CatalogSelection,
    Register,
    Raw,
    Stage,
    Continuity,
    Removal,
    RootRetirement,
    Unwind,
}

/// Complete returned evidence from the sole walker. On error the observer's
/// public wrapper exposes its raw failure, without partial coverage authority.
pub struct RecoveredNamespaceRead {
    pub entries: Vec<NativeObligation>,
    pub unresolved: Vec<NativeObligation>,
}

// The actual non-Clone prior effect is moved here before physical consumption.
// No serialized observation can construct this value or replace its CAS result.
struct PriorFacts {
    directory: Option<Arc<MediaDirectory>>,
    intent: Option<RestoreIntent>,
    populations: Vec<Result<NativeObligations, Box<RecoveryPopulationFailure>>>,
    register_attempted: bool,
    register: Option<Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>>,
    state: RecoveredRestoreState,
    refusal: Option<RecoveredRestoreRefusal>,
}

/// Closed custody even if the final consuming lower call unwinds. The original
/// root/Activity remain outside that call; no missing physical owner or receipt
/// is reconstructed. Drop only releases these handles and actual returned data.
#[must_use = "Retain the whole retirement result and original lease through handoff"]
pub struct RecoveredRetirementOutcome {
    root: Arc<JournalRoot>,
    activity: Arc<ActivityLease>,
    original: MediaRead,
    prior: Option<RecoveredRestoreOutcome>,
    transferred: Option<PriorFacts>,
    metadata: crate::restore_intent_store::recovered::Owner,
    root_attempted: bool,
    root_retirement: Option<RecoveredArchiveRetirement>,
    state: RecoveredRetirementState,
    refusal: Option<RecoveredRetirementRefusal>,
}

impl RecoveredRetirementOutcome {
    pub fn successor_write_attempted(&self) -> bool {
        self.metadata.write_attempted()
    }
    pub fn metadata_removal_attempted(&self) -> bool {
        self.metadata.removal_attempted()
    }
    pub fn metadata_stage_failure(&self) -> Option<&OwnedMediaFailure<Option<MediaStage>>> {
        self.metadata.stage_failure()
    }
    pub fn metadata_unused_stage(&self) -> Option<&MediaStage> {
        self.metadata.unused_stage()
    }
    pub fn state(&self) -> RecoveredRetirementState {
        self.state
    }
    pub fn disposition(&self) -> RestoreDisposition {
        if self.state == RecoveredRetirementState::Finished {
            RestoreDisposition::AlreadyPresent
        } else {
            RestoreDisposition::Unverified
        }
    }
    pub fn files_restored(&self) -> u64 {
        0
    }
    pub fn bytes_restored(&self) -> u64 {
        0
    }
    pub fn root(&self) -> &Arc<JournalRoot> {
        &self.root
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        &self.activity
    }
    pub fn original_record(&self) -> &MediaRead {
        &self.original
    }
    pub fn prior_register(
        &self,
    ) -> Option<&Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>> {
        self.prior
            .as_ref()
            .and_then(|prior| prior.register.as_ref())
            .or_else(|| {
                self.transferred
                    .as_ref()
                    .and_then(|facts| facts.register.as_ref())
            })
    }
    pub fn prior_populations(
        &self,
    ) -> &[Result<NativeObligations, Box<RecoveryPopulationFailure>>] {
        if let Some(prior) = &self.prior {
            &prior.populations
        } else {
            self.transferred
                .as_ref()
                .map_or(&[], |facts| &facts.populations)
        }
    }
    pub fn prior_directory(&self) -> Option<&Arc<MediaDirectory>> {
        self.prior
            .as_ref()
            .and_then(|prior| prior.directory.as_ref())
            .or_else(|| {
                self.transferred
                    .as_ref()
                    .and_then(|facts| facts.directory.as_ref())
            })
    }
    pub fn prior_intent(&self) -> Option<&RestoreIntent> {
        self.prior
            .as_ref()
            .and_then(|prior| prior.intent.as_ref())
            .or_else(|| {
                self.transferred
                    .as_ref()
                    .and_then(|facts| facts.intent.as_ref())
            })
    }
    pub fn prior_register_attempted(&self) -> bool {
        self.prior.as_ref().map_or_else(
            || {
                self.transferred
                    .as_ref()
                    .is_some_and(|facts| facts.register_attempted)
            },
            |prior| prior.register_attempted,
        )
    }
    pub fn prior_state(&self) -> RecoveredRestoreState {
        self.prior.as_ref().map_or_else(
            || {
                self.transferred
                    .as_ref()
                    .map_or(RecoveredRestoreState::Unknown, |facts| facts.state)
            },
            |prior| prior.state,
        )
    }
    pub fn prior_refusal(&self) -> Option<&RecoveredRestoreRefusal> {
        self.prior
            .as_ref()
            .and_then(|prior| prior.refusal.as_ref())
            .or_else(|| {
                self.transferred
                    .as_ref()
                    .and_then(|facts| facts.refusal.as_ref())
            })
    }
    pub fn register_attempted(&self) -> bool {
        self.metadata.repeated_attempted()
    }
    pub fn populations(&self) -> &[Result<NativeObligations, Box<RecoveryPopulationFailure>>] {
        self.metadata.populations()
    }
    pub fn metadata_named_absence(&self) -> Option<bool> {
        self.metadata.named_absence()
    }
    pub fn catalog_failure(&self) -> Option<RepairHold> {
        self.metadata.catalog_problem()
    }
    pub fn register_failure(
        &self,
    ) -> Option<&solstone_core_retention::review_context::ReviewRefusal> {
        self.metadata.register_problem()
    }
    pub fn register(&self) -> Option<&Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>> {
        self.metadata.repeated()
    }
    pub fn named_reads(&self) -> &[MediaRead] {
        self.metadata.named()
    }
    pub fn catalogs(&self) -> &[JournalCatalog] {
        self.metadata.catalogs()
    }
    pub fn registers(&self) -> &[Register] {
        self.metadata.registers()
    }
    pub fn raw_observations(&self) -> &[io::Result<Option<MediaObservation>>] {
        self.metadata.raw()
    }
    pub fn old_stage_observations(&self) -> &[io::Result<Option<Arc<MediaDirectory>>>] {
        self.metadata.old_stages()
    }
    pub fn decisions(&self) -> &[RecoveryDecision] {
        self.metadata.decisions()
    }
    pub fn walks(&self) -> &[Result<RecoveredNamespaceRead, RecoveryObservationError>] {
        self.metadata.walks()
    }
    pub fn successor(&self) -> Option<&RestoreIntent> {
        self.metadata.successor()
    }
    pub fn successor_confirmation(&self) -> Option<&MediaRead> {
        self.metadata.confirmation()
    }
    pub fn successor_durability(&self) -> Option<&[Option<Durability>; 4]> {
        self.metadata.durability()
    }
    #[cfg(unix)]
    pub fn successor_publication(
        &self,
    ) -> Option<
        Result<
            &solstone_core_journal_io::atomic::BoundAtomicOutcome,
            &solstone_core_journal_io::atomic::DetailedAtomicError,
        >,
    > {
        self.metadata.publication()
    }
    pub fn metadata_removal(&self) -> Option<&MediaRemoval> {
        self.metadata.removal()
    }
    pub fn metadata_removal_observation(&self) -> Option<&MediaRead> {
        self.metadata.removal_observation()
    }
    pub fn metadata_ancestor_durability(&self) -> Option<&[Durability; 4]> {
        self.metadata.ancestors()
    }
    pub fn store_io_failures(&self) -> impl Iterator<Item = &str> {
        self.metadata.failures()
    }
    pub fn root_retirement_attempted(&self) -> bool {
        self.root_attempted
    }
    pub fn root_retirement(&self) -> Option<&RecoveredArchiveRetirement> {
        self.root_retirement.as_ref()
    }
    pub fn refusal(&self) -> Option<&RecoveredRetirementRefusal> {
        self.refusal.as_ref()
    }
}

/// A separate admission with Missing never reaches this transition: only the
/// same opaque confirmed predecessor can pair its actual earlier Applied result.
pub fn retire_recovered_restore(
    prior: RecoveredRestoreOutcome,
    options: LockOptions,
) -> RecoveredRetirementOutcome {
    let mut result = RecoveredRetirementOutcome {
        root: Arc::clone(prior.reservation.root()),
        activity: Arc::clone(prior.reservation.activity()),
        original: prior.reservation.record().clone(),
        prior: Some(prior),
        transferred: None,
        metadata: Default::default(),
        root_attempted: false,
        root_retirement: None,
        state: RecoveredRetirementState::Held,
        refusal: None,
    };
    match catch_unwind(AssertUnwindSafe(|| execute(&mut result, options))) {
        Ok(Ok(())) => result.state = RecoveredRetirementState::Finished,
        Ok(Err(refusal)) => result.refusal = Some(refusal),
        Err(_) => {
            result.state = RecoveredRetirementState::Unknown;
            result.refusal = Some(RecoveredRetirementRefusal::Unwind);
        }
    }
    result
}

fn execute(
    result: &mut RecoveredRetirementOutcome,
    options: LockOptions,
) -> Result<(), RecoveredRetirementRefusal> {
    let prior = result
        .prior
        .as_ref()
        .ok_or(RecoveredRetirementRefusal::PriorHeld)?;
    if prior.state != RecoveredRestoreState::RegisterConfirmed {
        return Err(RecoveredRetirementRefusal::PriorHeld);
    }
    result.metadata.execute(prior, options)?;
    // Move every actual prior fact outside the final call before transferring
    // its single physical owner. Unknown keeps the original outside exclusion.
    let RecoveredRestoreOutcome {
        reservation,
        directory,
        intent,
        populations,
        register_attempted,
        register,
        state,
        refusal,
    } = result
        .prior
        .take()
        .ok_or(RecoveredRetirementRefusal::PriorHeld)?;
    result.transferred = Some(PriorFacts {
        directory,
        intent,
        populations,
        register_attempted,
        register,
        state,
        refusal,
    });
    result.root_attempted = true;
    result.root_retirement = Some(reservation.retire_after_settlement());
    if result
        .root_retirement
        .as_ref()
        .is_some_and(RecoveredArchiveRetirement::is_complete)
    {
        Ok(())
    } else {
        Err(RecoveredRetirementRefusal::RootRetirement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_retirement_is_send_static() {
        fn require<T: Send + 'static>() {}
        require::<RecoveredRetirementOutcome>();
    }
}
