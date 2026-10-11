// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Conditional retirement of this process's exact confirmed metadata record.
//! Root WAN-carrier settlement and retirement belong to the lower owner.

use super::*;
use solstone_core_journal_io::media::{MediaRead, MediaRemoval};

pub(super) enum NativeRetirement {
    None,
    StageFailure(OwnedMediaFailure<Option<MediaStage>>),
    Unconsumed(MediaStage),
    Removal(Box<MediaRemoval>),
}

pub(super) struct RetirementFacts {
    pub(super) observed: MediaRead,
    pub(super) native: NativeRetirement,
    pub(super) ancestors: [Durability; 4],
    pub(super) failures: Vec<StoreProblem>,
    pub(super) retired: bool,
}

impl TransactionOutcome<'_, '_> {
    /// Only an actual confirmed in-process store write supplies record continuity.
    /// Replay without that native observation needs separately qualified adoption.
    pub(crate) fn retire_intent(mut self, options: LockOptions) -> Self {
        if self.hold.is_some() || self.retirement.is_some() {
            return self;
        }
        #[cfg(unix)]
        {
            self = self.retire_with(options, &mut super::super::unix::NativeHooks);
        }
        #[cfg(not(unix))]
        {
            let _ = options;
            self.hold = Some(Hold::Store(StoreProblem::Unsupported));
        }
        self
    }

    #[cfg(unix)]
    pub(super) fn retire_with(
        mut self,
        options: LockOptions,
        hooks: &mut impl super::super::unix::Hooks,
    ) -> Self {
        let result = self.retire_bound(options, hooks);
        if let Err(hold) = result {
            self.hold = Some(hold);
        }
        self
    }

    #[cfg(unix)]
    fn retire_bound(
        &mut self,
        options: LockOptions,
        hooks: &mut impl super::super::unix::Hooks,
    ) -> Result<(), Hold> {
        use sha2::{Digest, Sha256};
        use solstone_core_journal_io::activity_obligations::{
            IntentRead, NativeObligation, intent_filename,
        };
        use solstone_core_journal_io::media::{Cleanup, OriginalName, RemovalEffect};
        use std::path::Path;

        self.owner.native_check()?;
        self.owner.admission.check()?;
        let record = self.record()?;
        let intent = &record.intent;
        if record.persisted.as_ref() != Some(intent)
            || intent.marker != Obligation::Complete
            || intent.event_recorded != Obligation::Complete
            || !matches!(
                intent.register,
                Obligation::Complete | Obligation::NotRequired
            )
            || intent.cleanup != Obligation::Complete
            || !intent.event.files.iter().all(|file| {
                matches!(
                    file.disposition,
                    RestoreDisposition::Published | RestoreDisposition::AlreadyPresent
                )
            })
            || intent.outcome.as_ref().is_none_or(|facts| {
                facts.confirmation != Confirmation::Confirmed
                    || (facts.effect == PublicationEffect::Published
                        && (facts.file_durability != Durability::Synced
                            || facts.parent_durability != Durability::Synced))
            })
        {
            return Err(Hold::Retirement);
        }
        let confirmed = record
            .writes
            .last()
            .filter(|write| write.fully_confirmed())
            .and_then(|write| write.confirmation.as_ref())
            .ok_or(Hold::Retirement)?
            .clone();
        let claims = OutputClaims::acquire(
            intent.output_paths(self.owner.root().canonical_path()),
            options,
        )
        .map_err(|_| Hold::Repair(RepairHold::ClaimsUnavailable))?;
        let commit = claims
            .commit(self.owner.destination.live().diagnostic_path())
            .map_err(|_| Hold::Repair(RepairHold::DestinationChanged))?;
        if claims
            .segment_identity(self.owner.destination.live().diagnostic_path())
            .map_err(|_| Hold::Repair(RepairHold::DestinationChanged))?
            != self.owner.destination.live().object_identity()
        {
            return Err(Hold::Repair(RepairHold::DestinationChanged));
        }
        self.owner.effect_check(&commit)?;
        self.live_matches()?;
        let decision = self.inspect()?;
        if !decision.event_recorded
            || !matches!(
                decision.register,
                RegisterDecision::NoMark | RegisterDecision::Complete
            )
        {
            return Err(Hold::Retirement);
        }
        let record = self.record()?;
        let store = record.store.as_ref().ok_or(Hold::Retirement)?;
        let observed = store
            .read(&record.intent.event.operation_id)
            .map_err(Hold::Store)?
            .ok_or(Hold::Retirement)?;
        if observed.intent != record.intent || observed.read != confirmed {
            return Err(Hold::Store(StoreProblem::Changed));
        }
        let name = intent_filename(&observed.intent.event.operation_id).ok_or(Hold::Retirement)?;
        let relative = Path::new("health/offload/restore-intents").join(&name);
        let census = observe_with_carrier(self.owner.root(), self.owner.carrier.observation())
            .map_err(|error| Hold::Admission(error.to_string()))?;
        let mut matched = 0_usize;
        for entry in census.unresolved_entries() {
            if matches!(entry, NativeObligation::Intent { relative: current, entry, contents:
                IntentRead::Complete { operation_id, bytes } }
                if *current == relative && *entry == observed.read.entry
                    && *operation_id == observed.intent.event.operation_id
                    && *bytes == observed.read.bytes)
            {
                matched += 1;
            } else {
                return Err(Hold::Obligations);
            }
        }
        if matched != 1 {
            return Err(Hold::Obligations);
        }
        drop(census);
        self.retirement = Some(RetirementFacts {
            observed: observed.read.clone(),
            native: NativeRetirement::None,
            ancestors: [Durability::Unproven; 4],
            failures: Vec::new(),
            retired: false,
        });
        let store = self.record()?.store.as_ref().ok_or(Hold::Retirement)?;
        store.revalidate().map_err(Hold::Store)?;
        let mut bindings = store.chain.iter();
        let _health = bindings.next().ok_or(Hold::Retirement)?;
        let parent = Arc::clone(
            bindings
                .next()
                .and_then(|binding| binding.media.as_ref())
                .ok_or(Hold::Retirement)?,
        );
        let source = Arc::clone(
            bindings
                .next()
                .and_then(|binding| binding.media.as_ref())
                .ok_or(Hold::Retirement)?,
        );
        self.owner.effect_check(&commit)?;
        let stage = match MediaStage::create(parent, Arc::clone(self.owner.carrier.activity())) {
            Ok(stage) => stage,
            Err(failure) => {
                self.retirement.as_mut().ok_or(Hold::Retirement)?.native =
                    NativeRetirement::StageFailure(failure);
                return Err(Hold::Retirement);
            }
        };
        self.retirement.as_mut().ok_or(Hold::Retirement)?.native =
            NativeRetirement::Unconsumed(stage);
        let expected = source
            .observe_file(&name)
            .map_err(|_| Hold::Retirement)?
            .ok_or(Hold::Retirement)?;
        let current = self
            .record()?
            .store
            .as_ref()
            .ok_or(Hold::Retirement)?
            .read(&observed.intent.event.operation_id)
            .map_err(Hold::Store)?
            .ok_or(Hold::Retirement)?;
        if current.read != confirmed
            || current.intent != observed.intent
            || expected.identity != confirmed.entry.identity
            || expected.digest.size != confirmed.entry.size
            || expected.digest.sha256 != format!("{:x}", Sha256::digest(&confirmed.bytes))
        {
            return Err(Hold::Store(StoreProblem::Changed));
        }
        self.record()?
            .store
            .as_ref()
            .ok_or(Hold::Retirement)?
            .revalidate()
            .map_err(Hold::Store)?;
        self.owner.effect_check(&commit)?;
        let native = &mut self.retirement.as_mut().ok_or(Hold::Retirement)?.native;
        let NativeRetirement::Unconsumed(stage) = std::mem::replace(native, NativeRetirement::None)
        else {
            return Err(Hold::Retirement);
        };
        *native =
            NativeRetirement::Removal(Box::new(source.remove_observed(&name, &expected, stage)));
        let (ancestors, failures) = self
            .record()?
            .store
            .as_ref()
            .ok_or(Hold::Retirement)?
            .sync_facts(hooks);
        let facts = self.retirement.as_mut().ok_or(Hold::Retirement)?;
        facts.ancestors = ancestors;
        facts.failures = failures;
        let confirmed_removal = matches!(&facts.native, NativeRetirement::Removal(removal)
            if removal.receipt().effect == RemovalEffect::Deleted
                && removal.receipt().original_name == OriginalName::Absent
                && removal.receipt().claim_confirmation == Confirmation::Confirmed
                && removal.receipt().file_durability == Durability::Synced
                && removal.receipt().source_durability == Durability::Synced
                && removal.receipt().claim_durability == Durability::Synced
                && removal.receipt().stage_parent_durability == Durability::Synced
                && removal.receipt().cleanup == Cleanup::Complete
                && removal.receipt().failures.is_empty());
        if !confirmed_removal || ancestors != [Durability::Synced; 4] || !facts.failures.is_empty()
        {
            return Err(Hold::Retirement);
        }
        self.record()?
            .store
            .as_ref()
            .ok_or(Hold::Retirement)?
            .revalidate()
            .map_err(Hold::Store)?;
        self.owner.native_check()?;
        let census = observe_with_carrier(self.owner.root(), self.owner.carrier.observation())
            .map_err(|error| Hold::Admission(error.to_string()))?;
        if census.unresolved_entries().next().is_some() {
            return Err(Hold::Obligations);
        }
        self.retirement.as_mut().ok_or(Hold::Retirement)?.retired = true;
        Ok(())
    }
}
