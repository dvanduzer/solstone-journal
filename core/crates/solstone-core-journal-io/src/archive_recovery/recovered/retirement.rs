// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::media::{
    ActivityCarrierRetirementEffect, ActivityCarrierRetirementReceipt, Confirmation, Durability,
};

/// Closed custody of the original physical owner and independent native facts.
/// This is never evidence of domain settlement or namespace cooperation. Drop
/// closes handles only; there is no retry, execution or owner extraction entry.
#[must_use = "Retain actual retirement facts and original lease through handoff"]
pub struct RecoveredArchiveRetirement {
    owner: RecoveredArchiveReservation,
    receipt: ActivityCarrierRetirementReceipt,
    before: Option<NativeObligations>,
    before_incomplete: Option<Box<IncompleteRecoveryObservation>>,
    after: Option<NativeObligations>,
    after_incomplete: Option<Box<IncompleteRecoveryObservation>>,
    named_reads: Vec<MediaRead>,
    selected_absence: Confirmation,
    refusals: Vec<RecoveryAdmissionError>,
}

#[cfg(target_os = "linux")]
pub(super) enum RetirementCheckpoint {
    BeforeFirstWalk,
    BeforeRemoval,
    BeforeFinalWalk,
    AfterFinalWalk,
}

impl RecoveredArchiveReservation {
    /// Neutral native retirement. The consuming caller must have actually
    /// settled its writers/effects and qualified namespace cooperation first.
    /// Unknown premises hold; no supplied value or reference count proves them.
    pub fn retire_after_settlement(self) -> RecoveredArchiveRetirement {
        #[cfg(target_os = "linux")]
        {
            self.retire_with(|_| {})
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut result = RecoveredArchiveRetirement::new(self);
            result.refusals.push(RecoveryAdmissionError::Unsupported);
            result
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn retirement_directories(&self) -> (&MediaDirectory, &MediaDirectory, &OsStr) {
        (
            self.resources
                .root_directory
                .as_deref()
                .expect("actual root retained"),
            self.resources
                .reservation
                .as_deref()
                .expect("actual reservation retained"),
            OsStr::new(self.binding().reservation_name()),
        )
    }

    // Only the actual Removed syscall branch uses this validator. It checks the
    // same original context without demanding that the removed child still exist.
    #[cfg(target_os = "linux")]
    pub(crate) fn revalidate_after_removal(&self) -> Result<(), RecoveryAdmissionError> {
        self.resources.revalidate_guards()?;
        self.resources.revalidate_root_directory()?;
        self.resources.revalidate_record_chain()?;
        self.activity()
            .revalidate()
            .map_err(RecoveryAdmissionError::Activity)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn retire_with(
        self,
        mut checkpoint: impl FnMut(RetirementCheckpoint),
    ) -> RecoveredArchiveRetirement {
        let mut result = RecoveredArchiveRetirement::new(self);
        let ready = (|| {
            result.owner.revalidate()?;
            result.check_selected_absence()?;
            checkpoint(RetirementCheckpoint::BeforeFirstWalk);
            result.walk(false)?;
            let matched = result
                .owner
                .observation()
                .validate(result.owner.root())
                .map_err(RecoveryAdmissionError::Native)?;
            let census = result
                .before
                .as_ref()
                .expect("actual before census retained");
            activity_obligations::matching_carrier(
                census.entries(),
                &matched.name,
                matched.identity,
            )
            .map_err(|error| RecoveryAdmissionError::Native(io::Error::other(error)))?;
            if census.entries().len() != 1 {
                return Err(Resources::native(
                    "retirement census contains other obligations",
                ));
            }
            result.owner.revalidate()?;
            result.check_selected_absence()?;
            result.owner.revalidate()?;
            checkpoint(RetirementCheckpoint::BeforeRemoval);
            Ok(())
        })();
        if let Err(error) = ready {
            result.refusals.push(error);
            return result;
        }
        result.receipt = crate::media::retire_recovered(&result.owner);
        if result.receipt.effect() != ActivityCarrierRetirementEffect::Removed {
            return result;
        }
        // Preserve independent late observations even when one already refuses.
        if let Err(error) = result.owner.revalidate_after_removal() {
            result.refusals.push(error);
        }
        if let Err(error) = result.check_selected_absence() {
            result.refusals.push(error);
        }
        checkpoint(RetirementCheckpoint::BeforeFinalWalk);
        if let Err(error) = result.walk(true) {
            result.refusals.push(error);
        }
        checkpoint(RetirementCheckpoint::AfterFinalWalk);
        // The walker validates JournalRoot, not the original Activity or record
        // chain. Its returned census must survive these separate trailing checks.
        if let Err(error) = result.owner.revalidate_after_removal() {
            result.refusals.push(error);
        }
        if let Err(error) = result.check_selected_absence() {
            result.refusals.push(error);
        }
        if let Err(error) = result.owner.revalidate_after_removal() {
            result.refusals.push(error);
        }
        result
    }
}

impl RecoveredArchiveRetirement {
    fn new(owner: RecoveredArchiveReservation) -> Self {
        Self {
            owner,
            receipt: ActivityCarrierRetirementReceipt::not_attempted(),
            before: None,
            before_incomplete: None,
            after: None,
            after_incomplete: None,
            named_reads: Vec::new(),
            selected_absence: Confirmation::Unverified,
            refusals: Vec::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn check_selected_absence(&mut self) -> Result<(), RecoveryAdmissionError> {
        self.selected_absence = Confirmation::Unverified;
        let parent = self
            .owner
            .resources
            .record_chain
            .last()
            .expect("fixed chain retained");
        let name = self
            .owner
            .resources
            .record_name
            .as_ref()
            .expect("plain selector retained");
        if let Some(read) = parent
            .read_file_bounded(name, MAX_INTENT_BYTES)
            .map_err(RecoveryAdmissionError::Native)?
        {
            self.named_reads.push(read);
            return Err(Resources::native(
                "selected recovery record remains occupied",
            ));
        }
        self.selected_absence = Confirmation::Confirmed;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn walk(&mut self, final_walk: bool) -> Result<(), RecoveryAdmissionError> {
        match activity_obligations::observe_retained(self.owner.root()) {
            Ok(census) => {
                let empty = census.entries().is_empty();
                if final_walk {
                    self.after = Some(census);
                    if !empty {
                        return Err(Resources::native(
                            "final retirement census contains obligations",
                        ));
                    }
                } else {
                    self.before = Some(census);
                }
                Ok(())
            }
            Err(incomplete) => {
                if final_walk {
                    self.after_incomplete = Some(incomplete);
                } else {
                    self.before_incomplete = Some(incomplete);
                }
                Err(RecoveryAdmissionError::IncompleteCensus)
            }
        }
    }

    pub fn root(&self) -> &Arc<JournalRoot> {
        self.owner.root()
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.activity()
    }
    pub fn binding(&self) -> &ArchiveRecoveryBinding {
        self.owner.binding()
    }
    pub fn operation_id(&self) -> &str {
        self.owner.operation_id()
    }
    pub fn record(&self) -> &MediaRead {
        self.owner.record()
    }
    pub fn admission_census(&self) -> &NativeObligations {
        self.owner.census()
    }
    pub fn receipt(&self) -> &ActivityCarrierRetirementReceipt {
        &self.receipt
    }
    pub fn before_census(&self) -> Option<&NativeObligations> {
        self.before.as_ref()
    }
    pub fn before_incomplete(&self) -> Option<&IncompleteRecoveryObservation> {
        self.before_incomplete.as_deref()
    }
    pub fn final_census(&self) -> Option<&NativeObligations> {
        self.after.as_ref()
    }
    pub fn final_incomplete(&self) -> Option<&IncompleteRecoveryObservation> {
        self.after_incomplete.as_deref()
    }
    pub fn named_reads(&self) -> &[MediaRead] {
        &self.named_reads
    }
    pub fn selected_absence(&self) -> Confirmation {
        self.selected_absence
    }
    pub fn refusals(&self) -> &[RecoveryAdmissionError] {
        &self.refusals
    }

    /// Native facts only; never establishes domain settlement/cooperation.
    pub fn is_complete(&self) -> bool {
        self.receipt.effect() == ActivityCarrierRetirementEffect::Removed
            && self.receipt.root_durability() == Durability::Synced
            && self.receipt.confirmation() == Confirmation::Confirmed
            && self.receipt.failures().is_empty()
            && self.refusals.is_empty()
            && self.selected_absence == Confirmation::Confirmed
            && self
                .after
                .as_ref()
                .is_some_and(|census| census.entries().is_empty())
    }
}
