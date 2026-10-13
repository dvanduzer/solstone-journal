// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Confirmed metadata retirement using the original recovered reservation.

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::io;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;

use solstone_core_journal_io::LockOptions;
use solstone_core_journal_io::activity_obligations::RecoveryObservationError;
#[cfg(target_os = "linux")]
use solstone_core_journal_io::activity_obligations::{
    IntentRead, NativeObligation, intent_filename, observe_with_carrier,
};
use solstone_core_journal_io::archive_recovery::RecoveryPopulationFailure;
#[cfg(target_os = "linux")]
use solstone_core_journal_io::media::{
    Cleanup, Confirmation, OriginalName, RemovalEffect, RemovalPhase,
};
use solstone_core_journal_io::media::{
    Durability, MediaDirectory, MediaObservation, MediaRead, MediaRemoval, MediaStage,
    OwnedMediaFailure,
};
#[cfg(target_os = "linux")]
use solstone_core_journal_io::paths::{PathOrDay, iter_segments};
use solstone_core_retention::archive_catalog::JournalCatalog;
#[cfg(target_os = "linux")]
use solstone_core_retention::archive_catalog::read_complete_catalog;
use solstone_core_retention::mark_mutation::{BoundMutationOutcome, RecoveredRestoredMarkRefusal};
#[cfg(target_os = "linux")]
use solstone_core_retention::mark_mutation::{
    MutationStatus, RecoveredRestoredMarkMutation, conditional_retire_recovered_restored,
};
#[cfg(target_os = "linux")]
use solstone_core_retention::restore_intent::{
    MarkRetirement, Obligation, RegisterDecision, content_observation, inspect_recovery,
    selected_name,
};
use solstone_core_retention::restore_intent::{RecoveryDecision, RepairHold, RestoreIntent};
use solstone_core_retention::{Register, review_context};
#[cfg(target_os = "linux")]
use solstone_core_segment::output_attempt::{OutputClaims, SegmentCommit};

#[cfg(target_os = "linux")]
use super::ObservedIntent;
use super::transaction::retirement::{NativeRetirement, RetirementFacts};
use super::{RestoreIntentStore, StoreProblem, StoreWrite};
use crate::restore_prepared::PreparedRestore;
use crate::restore_recovered::RecoveredRestoreOutcome;
use crate::restore_recovered::retirement::RecoveredNamespaceRead;
use crate::restore_recovered::retirement::RecoveredRetirementRefusal as Refusal;

#[derive(Default)]
pub(crate) struct Owner {
    store: Option<RestoreIntentStore>,
    write: Option<StoreWrite>,
    prepared: Option<PreparedRestore>,
    named: Vec<MediaRead>,
    catalogs: Vec<JournalCatalog>,
    registers: Vec<Register>,
    raw: Vec<io::Result<Option<MediaObservation>>>,
    old_stages: Vec<io::Result<Option<Arc<MediaDirectory>>>>,
    decisions: Vec<RecoveryDecision>,
    walks: Vec<Result<RecoveredNamespaceRead, RecoveryObservationError>>,
    repeated: Option<Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>>,
    repeated_attempted: bool,
    write_attempted: bool,
    removal_attempted: bool,
    retirement: Option<RetirementFacts>,
    store_problem: Option<StoreProblem>,
    catalog_problem: Option<RepairHold>,
    register_problem: Option<review_context::ReviewRefusal>,
    successor: Option<RestoreIntent>,
    populations: Vec<
        Result<
            solstone_core_journal_io::activity_obligations::NativeObligations,
            Box<RecoveryPopulationFailure>,
        >,
    >,
    named_absence: Option<bool>,
}

impl Owner {
    pub(crate) fn write_attempted(&self) -> bool {
        self.write_attempted
    }
    pub(crate) fn removal_attempted(&self) -> bool {
        self.removal_attempted
    }
    pub(crate) fn stage_failure(&self) -> Option<&OwnedMediaFailure<Option<MediaStage>>> {
        match &self.retirement.as_ref()?.native {
            NativeRetirement::StageFailure(failure) => Some(failure),
            _ => None,
        }
    }
    pub(crate) fn unused_stage(&self) -> Option<&MediaStage> {
        match &self.retirement.as_ref()?.native {
            NativeRetirement::Unconsumed(stage) => Some(stage),
            _ => None,
        }
    }
    pub(crate) fn populations(
        &self,
    ) -> &[Result<
        solstone_core_journal_io::activity_obligations::NativeObligations,
        Box<RecoveryPopulationFailure>,
    >] {
        &self.populations
    }
    pub(crate) fn named_absence(&self) -> Option<bool> {
        self.named_absence
    }
    pub(crate) fn catalog_problem(&self) -> Option<RepairHold> {
        self.catalog_problem
    }
    pub(crate) fn register_problem(&self) -> Option<&review_context::ReviewRefusal> {
        self.register_problem.as_ref()
    }
    pub(crate) fn named(&self) -> &[MediaRead] {
        &self.named
    }
    pub(crate) fn catalogs(&self) -> &[JournalCatalog] {
        &self.catalogs
    }
    pub(crate) fn registers(&self) -> &[Register] {
        &self.registers
    }
    pub(crate) fn raw(&self) -> &[io::Result<Option<MediaObservation>>] {
        &self.raw
    }
    pub(crate) fn old_stages(&self) -> &[io::Result<Option<Arc<MediaDirectory>>>] {
        &self.old_stages
    }
    pub(crate) fn decisions(&self) -> &[RecoveryDecision] {
        &self.decisions
    }
    pub(crate) fn walks(&self) -> &[Result<RecoveredNamespaceRead, RecoveryObservationError>] {
        &self.walks
    }
    pub(crate) fn repeated(
        &self,
    ) -> Option<&Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>> {
        self.repeated.as_ref()
    }
    pub(crate) fn repeated_attempted(&self) -> bool {
        self.repeated_attempted
    }
    pub(crate) fn successor(&self) -> Option<&RestoreIntent> {
        self.successor.as_ref()
    }
    pub(crate) fn confirmation(&self) -> Option<&MediaRead> {
        self.write.as_ref()?.receipt.confirmation.as_ref()
    }
    pub(crate) fn durability(&self) -> Option<&[Option<Durability>; 4]> {
        Some(&self.write.as_ref()?.receipt.durability)
    }
    pub(crate) fn removal(&self) -> Option<&MediaRemoval> {
        match &self.retirement.as_ref()?.native {
            NativeRetirement::Removal(removal) => Some(removal),
            _ => None,
        }
    }
    pub(crate) fn ancestors(&self) -> Option<&[Durability; 4]> {
        Some(&self.retirement.as_ref()?.ancestors)
    }
    pub(crate) fn removal_observation(&self) -> Option<&MediaRead> {
        Some(&self.retirement.as_ref()?.observed)
    }
    pub(crate) fn failures(&self) -> impl Iterator<Item = &str> {
        self.store_problem
            .iter()
            .chain(self.write.iter().flat_map(|write| &write.receipt.failures))
            .chain(self.retirement.iter().flat_map(|facts| &facts.failures))
            .filter_map(|problem| match problem {
                StoreProblem::Io(detail) => Some(detail.as_str()),
                _ => None,
            })
    }

    #[cfg(unix)]
    pub(crate) fn publication(
        &self,
    ) -> Option<
        Result<
            &solstone_core_journal_io::atomic::BoundAtomicOutcome,
            &solstone_core_journal_io::atomic::DetailedAtomicError,
        >,
    > {
        match self.write.as_ref()?.receipt.native.as_ref()? {
            super::unix::NativePublication::Update(result) => Some(result.as_ref()),
            _ => None,
        }
    }

    fn store(&self) -> Result<&RestoreIntentStore, Refusal> {
        self.write
            .as_ref()
            .map(|write| &write.owner)
            .or(self.store.as_ref())
            .ok_or(Refusal::Store)
    }

    fn store_result<T>(&mut self, result: Result<T, StoreProblem>) -> Result<T, Refusal> {
        result.map_err(|problem| {
            self.store_problem = Some(problem);
            Refusal::Store
        })
    }

    #[cfg(target_os = "linux")]
    fn check(
        &mut self,
        prior: &RecoveredRestoreOutcome,
        claims: &OutputClaims,
        commit: &SegmentCommit<'_>,
        intent: &RestoreIntent,
    ) -> Result<(), Refusal> {
        prior
            .reservation()
            .revalidate()
            .map_err(Refusal::Physical)?;
        let checked = self.store()?.revalidate();
        self.store_result(checked)?;
        let prepared = self.prepared.as_ref().ok_or(Refusal::Continuity)?;
        prepared.revalidate().map_err(|error| Refusal::Prepared {
            reason: error.reason,
            detail: error.detail,
        })?;
        commit
            .require_outputs(
                claims,
                prepared.live().diagnostic_path(),
                intent.output_paths(prior.reservation().root().canonical_path()),
            )
            .map_err(Refusal::Claims)?;
        if claims
            .segment_identity(prepared.live().diagnostic_path())
            .map_err(Refusal::Claims)?
            != prepared.live().object_identity()
        {
            return Err(Refusal::Intent(RepairHold::DestinationChanged));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn named_read(&mut self, operation: &str) -> Result<ObservedIntent, Refusal> {
        let result = self.store()?.read(operation);
        let observed = self.store_result(result)?.ok_or(Refusal::Continuity)?;
        self.named.push(observed.read.clone());
        Ok(observed)
    }

    #[cfg(target_os = "linux")]
    fn walk(
        &mut self,
        prior: &RecoveredRestoreOutcome,
        expected: Option<&MediaRead>,
    ) -> Result<(), Refusal> {
        let actual = observe_with_carrier(
            prior.reservation().root(),
            prior.reservation().observation(),
        )
        .map(|walk| RecoveredNamespaceRead {
            entries: walk.entries().to_vec(),
            unresolved: walk.unresolved_entries().cloned().collect(),
        });
        self.walks.push(actual);
        let walk = self
            .walks
            .last()
            .ok_or(Refusal::Population)?
            .as_ref()
            .map_err(|_| Refusal::Population)?;
        let valid = match expected {
            None => walk.unresolved.is_empty(),
            Some(read) => {
                let operation = &prior.reservation().record().entry.name;
                let relative = Path::new("health/offload/restore-intents").join(operation);
                matches!(walk.unresolved.as_slice(), [NativeObligation::Intent { relative: current, entry, contents: IntentRead::Complete { operation_id, bytes } }]
                    if *current == relative && *entry == read.entry && *operation_id == prior.reservation().operation_id() && *bytes == read.bytes)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Refusal::Population)
        }
    }

    #[cfg(target_os = "linux")]
    fn absent_stage(&mut self, intent: &RestoreIntent) -> Result<(), Refusal> {
        let parent = self
            .prepared
            .as_ref()
            .ok_or(Refusal::Continuity)?
            .stage_parent();
        let observed = parent.open_child(OsStr::new(&intent.stage.stage_name));
        let absent = matches!(&observed, Ok(None));
        self.old_stages.push(observed);
        if absent { Ok(()) } else { Err(Refusal::Stage) }
    }

    #[cfg(target_os = "linux")]
    fn semantic(
        &mut self,
        prior: &RecoveredRestoreOutcome,
        intent: &RestoreIntent,
        successor: bool,
    ) -> Result<(), Refusal> {
        let root = prior.reservation().root();
        self.catalogs
            .push(read_complete_catalog(root).map_err(Refusal::Catalog)?);
        let loaded = review_context::load_at_root(root);
        match loaded {
            Ok(register) => self.registers.push(register),
            Err(error) => {
                self.register_problem = Some(error);
                return Err(Refusal::Register);
            }
        }
        let catalog = self.catalogs.last().ok_or(Refusal::Continuity)?;
        let selected = match super::adoption::selected_catalog(catalog, intent) {
            Ok(selected) => selected,
            Err(error) => {
                self.catalog_problem = Some(error);
                return Err(Refusal::CatalogSelection);
            }
        };
        let register = self.registers.last().ok_or(Refusal::Register)?;
        let MarkRetirement::Exact { plan } = &intent.retirement else {
            return Err(Refusal::Register);
        };
        let related = super::transaction::related_mark(
            &intent.locator.target,
            &intent.witness.file.name,
            register,
        )
        .map_err(Refusal::Intent)?;
        if register.marks.contains_key(&plan.expected().id)
            || related.is_some()
            || plan
                .replacement()
                .is_some_and(|replacement| register.marks.get(&replacement.id) != Some(replacement))
        {
            return Err(Refusal::Register);
        }
        let prepared = self.prepared.as_ref().ok_or(Refusal::Continuity)?;
        self.raw
            .push(prepared.live().observe_file(selected_name(intent)));
        let raw = self
            .raw
            .last()
            .ok_or(Refusal::Raw)?
            .as_ref()
            .map_err(|_| Refusal::Raw)?
            .as_ref()
            .ok_or(Refusal::Raw)?;
        let saved = intent.confirmed.as_ref().ok_or(Refusal::Raw)?;
        if raw.identity != saved.identity || raw.digest != saved.digest {
            return Err(Refusal::Raw);
        }
        let decision = inspect_recovery(
            intent,
            prepared.destination(),
            selected,
            register,
            content_observation(intent, Ok(Some(raw.clone()))),
        )
        .map_err(Refusal::Intent)?;
        let valid = decision.event_recorded
            && decision.marker == Obligation::Complete
            && if successor {
                decision.register == RegisterDecision::Complete
                    && decision.cleanup == Obligation::Complete
            } else if plan.replacement().is_none() {
                decision.register == RegisterDecision::Superseded
            } else {
                decision.register == RegisterDecision::AlreadyApplied
            };
        self.decisions.push(decision);
        if valid {
            Ok(())
        } else {
            Err(Refusal::Register)
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn execute(
        &mut self,
        _: &RecoveredRestoreOutcome,
        _: LockOptions,
    ) -> Result<(), Refusal> {
        Err(Refusal::Unsupported)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn execute(
        &mut self,
        prior: &RecoveredRestoreOutcome,
        options: LockOptions,
    ) -> Result<(), Refusal> {
        #[cfg(all(test, feature = "full-tests"))]
        let mut hooks = tests::Hooks;
        #[cfg(not(all(test, feature = "full-tests")))]
        let mut hooks = super::unix::NativeHooks;
        self.execute_with(prior, options, &mut hooks)
    }

    #[cfg(target_os = "linux")]
    fn execute_with(
        &mut self,
        prior: &RecoveredRestoreOutcome,
        options: LockOptions,
        hooks: &mut impl super::unix::Hooks,
    ) -> Result<(), Refusal> {
        let population = prior.reservation().revalidate_current_population();
        let complete = population.is_ok();
        self.populations.push(population);
        if !complete {
            return Err(Refusal::Population);
        }
        let intent = prior.intent().ok_or(Refusal::Continuity)?.clone();
        let root = prior.reservation().root();
        let segment = iter_segments(
            root.canonical_path(),
            PathOrDay::Day(&intent.locator.target.day),
        )
        .map_err(|error| Refusal::Destination(io::Error::other(error)))?
        .into_iter()
        .find(|segment| {
            segment.path()
                == root
                    .canonical_path()
                    .join(intent.locator.relative_segment())
        })
        .ok_or(Refusal::Continuity)?;
        let prepared =
            PreparedRestore::admit(Arc::clone(root), &intent.locator.target.day, &segment)
                .map_err(|error| Refusal::Prepared {
                    reason: error.reason,
                    detail: error.detail,
                })?;
        if prepared.locator() != &intent.locator || prepared.destination() != &intent.destination {
            self.prepared = Some(prepared);
            return Err(Refusal::Intent(RepairHold::DestinationChanged));
        }
        self.prepared = Some(prepared);
        match RestoreIntentStore::open_existing(Arc::clone(prior.reservation().activity())) {
            Ok(store) => self.store = Some(store),
            Err(failure) => {
                self.store = Some(*failure.owner);
                self.store_problem = Some(failure.problem);
                return Err(Refusal::Store);
            }
        }
        let original = self.named_read(&intent.event.operation_id)?;
        if original.read != *prior.reservation().record() || original.intent != intent {
            return Err(Refusal::Continuity);
        }
        let outputs = intent.output_paths(root.canonical_path());
        let claims = OutputClaims::acquire(outputs.clone(), options).map_err(Refusal::Claims)?;
        let live = Arc::clone(self.prepared.as_ref().ok_or(Refusal::Continuity)?.live());
        let commit = claims
            .commit(live.diagnostic_path())
            .map_err(Refusal::Claims)?;
        self.check(prior, &claims, &commit, &intent)?;
        self.repeated_attempted = true;
        self.repeated = Some(conditional_retire_recovered_restored(
            RecoveredRestoredMarkMutation {
                reservation: prior.reservation(),
                claims: &claims,
                commit: &commit,
            },
        ));
        if !paired_confirmation(
            prior,
            self.repeated.as_ref().ok_or(Refusal::Register)?,
            &intent,
        ) {
            return Err(Refusal::Register);
        }
        self.check(prior, &claims, &commit, &intent)?;
        self.semantic(prior, &intent, false)?;
        self.absent_stage(&intent)?;
        self.walk(prior, Some(&original.read))?;
        self.check(prior, &claims, &commit, &intent)?;
        let current = self.named_read(&intent.event.operation_id)?;
        if current.read != original.read || current.intent != intent {
            return Err(Refusal::Continuity);
        }
        let mut successor = intent.clone();
        if successor.register == Obligation::Pending {
            successor.register = Obligation::Complete;
        }
        if successor.cleanup == Obligation::Pending {
            successor.cleanup = Obligation::Complete;
        }
        intent
            .validate_successor(&successor)
            .map_err(Refusal::Intent)?;
        self.successor = Some(successor.clone());
        let store = self.store.take().ok_or(Refusal::Store)?;
        self.write_attempted = true;
        self.write = Some(store.write_bound(successor.clone(), Some(&current), hooks));
        if !self
            .write
            .as_ref()
            .is_some_and(|write| write.receipt.fully_confirmed())
        {
            return Err(Refusal::Store);
        }
        #[cfg(all(test, feature = "full-tests"))]
        tests::checkpoint(tests::Checkpoint::SuccessorReturned);
        let confirmed = self.confirmation().ok_or(Refusal::Continuity)?.clone();
        self.check(prior, &claims, &commit, &successor)?;
        let current = self.named_read(&intent.event.operation_id)?;
        if current.read != confirmed || current.intent != successor {
            return Err(Refusal::Continuity);
        }
        self.walk(prior, Some(&confirmed))?;
        self.semantic(prior, &successor, true)?;
        self.absent_stage(&intent)?;
        self.check(prior, &claims, &commit, &successor)?;
        self.remove(prior, &claims, &commit, &successor, &confirmed, hooks)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn remove(
        &mut self,
        prior: &RecoveredRestoreOutcome,
        claims: &OutputClaims,
        commit: &SegmentCommit<'_>,
        intent: &RestoreIntent,
        confirmed: &MediaRead,
        _hooks: &mut impl super::unix::Hooks,
    ) -> Result<(), Refusal> {
        use sha2::{Digest, Sha256};
        self.retirement = Some(RetirementFacts {
            observed: confirmed.clone(),
            native: NativeRetirement::None,
            ancestors: [Durability::Unproven; 4],
            failures: Vec::new(),
            retired: false,
        });
        let chain = &self.store()?.chain;
        let parent = Arc::clone(
            chain
                .get(1)
                .and_then(|binding| binding.media.as_ref())
                .ok_or(Refusal::Store)?,
        );
        let source = Arc::clone(
            chain
                .get(2)
                .and_then(|binding| binding.media.as_ref())
                .ok_or(Refusal::Store)?,
        );
        let stage = match MediaStage::create(parent, Arc::clone(prior.reservation().activity())) {
            Ok(stage) => stage,
            Err(failure) => {
                self.retirement.as_mut().ok_or(Refusal::Removal)?.native =
                    NativeRetirement::StageFailure(failure);
                return Err(Refusal::Removal);
            }
        };
        self.retirement.as_mut().ok_or(Refusal::Removal)?.native =
            NativeRetirement::Unconsumed(stage);
        let name = intent_filename(&intent.event.operation_id).ok_or(Refusal::Continuity)?;
        let expected = source
            .observe_file(&name)
            .map_err(Refusal::Destination)?
            .ok_or(Refusal::Continuity)?;
        let named = self.named_read(&intent.event.operation_id)?;
        if named.read != *confirmed
            || named.intent != *intent
            || expected.identity != confirmed.entry.identity
            || expected.digest.size != confirmed.entry.size
            || expected.digest.sha256 != format!("{:x}", Sha256::digest(&confirmed.bytes))
        {
            return Err(Refusal::Continuity);
        }
        self.check(prior, claims, commit, intent)?;
        let native = &mut self.retirement.as_mut().ok_or(Refusal::Removal)?.native;
        let NativeRetirement::Unconsumed(stage) = std::mem::replace(native, NativeRetirement::None)
        else {
            return Err(Refusal::Removal);
        };
        self.removal_attempted = true;
        let mut removal = Box::new(source.remove_observed(&name, &expected, stage));
        if removal.receipt().cleanup != Cleanup::Complete {
            removal.cleanup();
        }
        let source_durability = removal.receipt().source_durability;
        *native = NativeRetirement::Removal(removal);
        #[cfg(all(test, feature = "full-tests"))]
        tests::checkpoint(tests::Checkpoint::AfterRemoval);
        let prep_durability = self.store()?.preparation_durability();
        let ancestors = [
            source_durability,
            prep_durability[1],
            prep_durability[2],
            prep_durability[3],
        ];
        let facts = self.retirement.as_mut().ok_or(Refusal::Removal)?;
        facts.ancestors = ancestors;
        facts.failures = Vec::new();
        if !matches!(&facts.native, NativeRetirement::Removal(removal) if removal_complete(removal))
            || ancestors != [Durability::Synced; 4]
            || !facts.failures.is_empty()
        {
            return Err(Refusal::Removal);
        }
        self.check(prior, claims, commit, intent)?;
        let absent = self.store()?.read(&intent.event.operation_id);
        let absent = self.store_result(absent)?;
        self.named_absence = Some(absent.is_none());
        if let Some(observed) = absent {
            self.named.push(observed.read);
            return Err(Refusal::Continuity);
        }
        self.walk(prior, None)?;
        self.check(prior, claims, commit, intent)?;
        self.retirement.as_mut().ok_or(Refusal::Removal)?.retired = true;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn paired_confirmation(
    prior: &RecoveredRestoreOutcome,
    repeated: &Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>,
    intent: &RestoreIntent,
) -> bool {
    if super::super::restore_recovered::register_confirmed(repeated) {
        return true;
    }
    matches!((&intent.retirement, prior.register(), repeated),
        (MarkRetirement::Exact { plan }, Some(Ok(BoundMutationOutcome::Published(first))), Ok(BoundMutationOutcome::Unchanged(current)))
        if plan.replacement().is_none() && first.status == MutationStatus::Applied && current.status == MutationStatus::Missing && !current.register.marks.contains_key(&plan.expected().id))
}

#[cfg(target_os = "linux")]
fn removal_complete(removal: &MediaRemoval) -> bool {
    let receipt = removal.receipt();
    receipt.effect == RemovalEffect::Deleted
        && receipt.original_name == OriginalName::Absent
        && receipt.claim_confirmation == Confirmation::Confirmed
        && receipt.source_durability == Durability::Synced
        && receipt.claim_durability == Durability::Synced
        && !receipt
            .failures
            .iter()
            .any(|f| f.phase != RemovalPhase::Cleanup)
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
pub(crate) mod tests;
